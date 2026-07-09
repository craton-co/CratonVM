# RealmModelTest no-JIT timeout in Liquibase/Xerces XML schema parsing - FIXED

Status: fixed

Date fixed: 2026-07-09

## Summary

The CratonVM `--nojit` `RealmModelTest` watchdog no longer stops in the
Liquibase/Xerces XML schema parse path that previously dominated
`dbchangelog-3.2.xsd` loading.

The fix adds CratonVM native fast paths for the Xerces scanner/schema-loading
hotspots that the interpreter could not execute quickly enough:

- `XMLChar` name/space classification against the real JDK `CHARS` table;
- `XMLLimitAnalyzer` counter updates and reads;
- `XSSimpleTypeDecl.normalize`;
- `XSDHandler$XSDKey` hash/equality;
- `XMLEntityScanner.scanContent`, newline normalization, entity-limit updates,
  and whitespace skipping;
- Xerces opti DOM trivial getters used while building schema DOMs;
- `RangeToken.sortRanges`.

The VM override gate now forces those methods through the native path even when
the interpreter is running with JIT disabled.

## Validation

Focused Rust coverage:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-xerces-tests-20260709-004-native'
cargo test -p cratonvm-native-builtins xerces --lib

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-xerces-tests-20260709-004-vm'
cargo test -p cratonvm-vm xerces --lib
```

Result:

```text
cratonvm-native-builtins: 17 passed
cratonvm-vm: 2 passed
```

The unique release binary used for suite validation was:

```text
C:\craton\cargo-targets\keycloak-liquibase-xerces-20260709-003\release\cratonvm-keycloak-liquibase-xerces-20260709-009.exe
```

A 900-second direct stack-watchdog run no longer stopped in:

```text
XMLChangeLogSAXParser.parseToNode
XMLEntityScanner.scanQName
XMLEntityScanner.checkLimit
XMLLimitAnalyzer.addValue
```

Instead, it progressed into a later Liquibase checksum/status path:

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

The requested Keycloak suite-runner check with the same unique binary still hit
the 900-second watchdog for `org.keycloak.testsuite.model.RealmModelTest`, but
the residual is no longer the Liquibase/Xerces XML parse hotspot described by
this note.

## Follow-up

The still-open follow-up is tracked separately as:

```text
docs/known-issues/keycloak-model-liquibase-checksum-status-nojit-timeout.md
```
