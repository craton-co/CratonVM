# RealmModelTest no-JIT timeout in Liquibase update/checksum work

Status: open

Date observed: 2026-07-09, after fixing the Liquibase/Xerces XML parse timeout.

## Summary

`RealmModelTest` under CratonVM `--nojit` now gets past the earlier
Liquibase/Xerces XML schema parsing hotspot. The resurfaced stack in
`XMLEntityScanner.scanQName -> checkLimit -> XMLLimitAnalyzer.addValue` no
longer appears after the 2026-07-09 `scanQName` native follow-up.

The class still exceeds the 900-second suite-runner watchdog. The current run is
later in Liquibase update work: it has parsed the changelogs, is executing DDL
and checksum bookkeeping, and was still applying Keycloak schema updates near
`META-INF/jpa-changelog-authz-3.4.0.CR1` when the watchdog fired.

HotSpot `-Xint` previously passed the same class/list in 142.649s, so this
remains a CratonVM no-JIT throughput residual rather than a Keycloak functional
failure.

## Evidence

The unique CratonVM binary used for the current validation was:

```text
C:\craton\cargo-targets\keycloak-xerces-scanqname-20260709-002\release\cratonvm-keycloak-xerces-scanqname-20260709-004.exe
```

Suite-runner command shape:

```powershell
& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -Vm craton -Category others -Jit off `
  -ClassList 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\keycloak-model-realm-stw-20260708-001.tsv' `
  -RunName 'keycloak-xerces-scanqname-verify-20260709-004' `
  -Parallel 1 -TimeoutSec 900 `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' `
  -WorkDir 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite' `
  -Exe 'C:\craton\cargo-targets\keycloak-xerces-scanqname-20260709-002\release\cratonvm-keycloak-xerces-scanqname-20260709-004.exe'
```

Result:

```text
HANG 900.073s org.keycloak.testsuite.model.RealmModelTest
```

The latest run is no longer in Xerces XML schema parsing and no longer hits the
intermediate `skipString` empty-rawname failure or H2 `BitSet.clone` failure
that the `scanQName` fix exposed. The last useful progress is Liquibase update
execution:

```text
ChangeSet META-INF/jpa-changelog-3.3.0.xml::3.3.0::keycloak ran successfully
Running Changeset: META-INF/jpa-changelog-authz-3.4.0.CR1.xml::authz-3.4.0.CR1-resource-server-pk-change-part1::glavoie@gmail.com
ALTER TABLE PUBLIC.RESOURCE_SERVER_POLICY ADD RESOURCE_SERVER_CLIENT_ID VARCHAR(36)
```

Earlier evidence for this same residual family stopped in checksum/status
serialization:

```text
LiquibaseJpaUpdaterProvider.update
AbstractUpdateCommandStep.getStatusVisitor
StatusChangeLogIterator.run
ChangeSetStatus.<init>
ChangeSet.generateCheckSum
AbstractChange.generateCheckSum
StringChangeLogSerializer.serializeObject
AbstractChange$1.include
```

## Current hypothesis

This is the next no-JIT Liquibase throughput layer after schema parsing. The
likely hot areas are the update visitor, SQL statement execution through H2,
checksum generation between change sets, reflection/property inspection, and
string serialization. It should be investigated as a Liquibase/H2 update
throughput residual, not as another Xerces scanner or XML schema-loading issue.

## Next leads

- Instrument `UpdateVisitor`, `JdbcExecutor`, `StringChangeLogSerializer`, and
  `AbstractChange.generateCheckSum` under `--nojit`.
- Compare method counts and allocation churn against HotSpot `-Xint`.
- Check whether the visitor/status path repeats checksum generation for the same
  change sets, repeatedly traverses reflective metadata, or spends most of the
  time in H2 DDL/index execution.
- Keep the previously fixed Xerces XML parser intrinsics in place; the current
  evidence has already moved past that phase.
