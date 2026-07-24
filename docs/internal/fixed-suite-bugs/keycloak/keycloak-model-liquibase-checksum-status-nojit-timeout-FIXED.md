# Keycloak RealmModelTest no-JIT Liquibase checksum/status timeout fixed

Status: fixed

Date fixed: 2026-07-09

## Summary

`RealmModelTest` under CratonVM `--nojit` no longer stops in the Liquibase
checksum/status/update path. The fix chain keeps the earlier
`AbstractChange$1.include(Object,String,Object)` checksum filter native, adds
H2 DDL identity/hash shortcuts for the Liquibase migration hot path, and adds a
`ColumnConfig.getSerializableFieldValue(String)` native that reads fields
directly and formats date values without constructing a fresh
`ISODateFormat`/six `SimpleDateFormat` objects for every date field.

The previous terminal failures are gone:

- no 900-second suite watchdog in Liquibase;
- no `SnapshotGeneratorFactory.getGenerators` comparator
  `NoSuchMethodError: Object.compare(Object,Object)I`;
- no late `ChangeSetStatus`/`ColumnConfig` stack stuck in
  `ISODateFormat.<init>()`.

The class still fails later, but the remaining failure is a separate
Hibernate/H2 authentication residual tracked in
`keycloak-model-realmmodeltest-h2-auth-after-liquibase-nojit-FIXED.md`.

## Evidence

Focused tests:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-native-tests-20260709-011'
cargo test -p cratonvm-native-builtins --lib liquibase_checksum_tests -- --nocapture

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-h2tests-20260709-011'
cargo test -p cratonvm-native-builtins --lib h2_ -- --nocapture

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-vmtests-20260709-011'
cargo test -p cratonvm-vm --lib liquibase_checksum_force_native_covers_status_hotpath_intrinsics -- --nocapture

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-vmtests-h2-20260709-011'
cargo test -p cratonvm-vm --lib h2_liquibase_force_native_covers_ddl_hotpath_intrinsics -- --nocapture
```

Results:

```text
cratonvm-native-builtins liquibase_checksum_tests: 4 passed
cratonvm-native-builtins h2_: 4 passed
cratonvm-vm liquibase_checksum_force_native_covers_status_hotpath_intrinsics: 1 passed
cratonvm-vm h2_liquibase_force_native_covers_ddl_hotpath_intrinsics: 1 passed
```

Release binary:

```text
C:\craton\cargo-targets\keycloak-liquibase-columnconfig-20260709-012\release\cratonvm-keycloak-liquibase-columnconfig-20260709-012.exe
```

Suite runner:

```powershell
& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -Vm craton -Category others -Jit off `
  -ClassList 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\keycloak-model-realm-stw-20260708-001.tsv' `
  -RunName 'keycloak-liquibase-columnconfig-verify-20260709-012' `
  -Parallel 1 -TimeoutSec 900 `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' `
  -WorkDir 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite' `
  -Exe 'C:\craton\cargo-targets\keycloak-liquibase-columnconfig-20260709-012\release\cratonvm-keycloak-liquibase-columnconfig-20260709-012.exe' `
  -JdkHome 'C:\Program Files\Java\jdk-25'
```

Result:

```text
FAIL 881.085s org.keycloak.testsuite.model.RealmModelTest
```

This is a normal process failure, not the 900-second `HANG`. The stderr log
shows Liquibase completed all current Keycloak changelog work:

```text
ChangeSet META-INF/jpa-changelog-26.7.0.xml::26.7.0-outbox::keycloak ran successfully
Run:                        195
Total change sets:          195
Completed database update for changelog {0}
```

The terminal failure is later:

```text
ExceptionInInitializerError
caused by org.hibernate.service.spi.ServiceException:
Unable to create requested service [org.hibernate.engine.jdbc.env.spi.JdbcEnvironment]
due to: Error calling Driver.connect() [Wrong user name or password [28000-240]]
```

## Notes

The Liquibase intrinsics should stay narrow. They target app/library bytecode
that is demonstrably hot in this no-JIT suite path and are covered by focused
native registration and force-native tests.
