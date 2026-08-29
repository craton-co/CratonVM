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
- `XMLEntityScanner.scanQName`, `scanContent`, newline normalization,
  entity-limit updates, and whitespace skipping;
- Xerces opti DOM trivial getters used while building schema DOMs;
- `RangeToken.sortRanges`.

The VM override gate now forces those methods through the native path even when
the interpreter is running with JIT disabled.

The 2026-07-09 follow-up also pins the `scanQName` symbol-table results until
they are installed into the target `QName`. Without that, a later symbol-table
call could move the raw name before the element stack copied it, surfacing as a
downstream `XMLEntityScanner.skipString` `StringIndexOutOfBoundsException`.

## Validation

Focused Rust coverage:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-xerces-tests-20260709-004-native'
cargo test -p cratonvm-native-builtins xerces --lib

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-xerces-tests-20260709-004-vm'
cargo test -p cratonvm-vm xerces --lib
```

Original result:

```text
cratonvm-native-builtins: 17 passed
cratonvm-vm: 2 passed
```

Follow-up coverage for the resurfaced `scanQName` stack:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-xerces-scanqname-tests-20260709-005-native'
cargo test -p cratonvm-native-builtins xerces --lib

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-xerces-scanqname-tests-20260709-005-vm'
cargo test -p cratonvm-vm object_clone_force_native --lib

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-xerces-scanqname-tests-20260709-006-vm-xerces'
cargo test -p cratonvm-vm xerces --lib
```

Result:

```text
cratonvm-native-builtins: 18 passed
cratonvm-vm object_clone_force_native: 1 passed
cratonvm-vm xerces: 2 passed
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

The follow-up unique binary was:

```text
C:\craton\cargo-targets\keycloak-xerces-scanqname-20260709-002\release\cratonvm-keycloak-xerces-scanqname-20260709-004.exe
```

The suite-runner check:

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

This run no longer shows:

```text
XMLEntityScanner.scanQName -> checkLimit -> XMLLimitAnalyzer.addValue
XMLEntityScanner.skipString -> StringIndexOutOfBoundsException
BitSet.clone -> CloneNotSupportedException
```

It instead reaches Liquibase changelog execution and is still applying schema
updates near `../../../../apps/META-INF/jpa-changelog-authz-3.4.0.CR1` when the 900-second
watchdog fires.

## Follow-up

The Liquibase checksum/status follow-up was later fixed and retired as:

```text
docs/internal/fixed-suite-bugs/keycloak-model-liquibase-checksum-status-nojit-timeout-FIXED.md
```

The still-open `RealmModelTest` residual after Liquibase completion is tracked
separately as:

```text
docs/internal/fixed-suite-bugs/keycloak-model-realmmodeltest-h2-auth-after-liquibase-nojit-FIXED.md
```
