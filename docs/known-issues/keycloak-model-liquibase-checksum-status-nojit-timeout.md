# RealmModelTest no-JIT Liquibase SnapshotGeneratorFactory comparator NSME

Status: open

Date observed: 2026-07-09, after fixing the Liquibase/Xerces parse timeout and
the Liquibase checksum/status filter hot leaf.

## Summary

`RealmModelTest` under CratonVM `--nojit` now gets past the earlier
Liquibase/Xerces XML schema parsing hotspot. The resurfaced stack in
`XMLEntityScanner.scanQName -> checkLimit -> XMLLimitAnalyzer.addValue` no
longer appears after the 2026-07-09 `scanQName` native follow-up.

This branch also removes one checksum/status interpreter hot layer by forcing
`liquibase/change/AbstractChange$1.include(Object,String,Object)` to the
native registry. The native implementation directly scans
`AbstractChange.getExcludedFieldFilters(...)` instead of interpreting the
per-field `Arrays.stream(...).anyMatch(...)` predicate for every serialized
change field.

With those layers in place, the class no longer hits the 900-second watchdog.
The latest current-dev validation exits in 851.630s with a later Liquibase
failure:

```text
java.lang.NoSuchMethodError: java/lang/Object.compare(Ljava/lang/Object;Ljava/lang/Object;)I
  at liquibase.snapshot.SnapshotGeneratorFactory.getGenerators(SnapshotGeneratorFactory.java:125)
```

The open residual is now a lambda/SAM dispatch bug in Liquibase's
`SnapshotGeneratorFactory` comparator chain, not the old XML parse timeout and
not the `AbstractChange$1.include` checksum filter loop.

HotSpot `-Xint` previously passed the same class/list in 142.649s, so this
remains a CratonVM correctness/dispatch residual.

## Evidence

The unique CratonVM binary used for the latest validation was:

```text
C:\craton\cargo-targets\keycloak-liquibase-checksum-status-20260709-003\release\cratonvm-keycloak-liquibase-checksum-status-20260709-003.exe
```

Focused native/dispatch tests:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-devtests-20260709-008'
cargo test -p cratonvm-native-builtins --lib liquibase_checksum -- --nocapture

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-vmtests-20260709-008'
cargo test -p cratonvm-vm --lib liquibase_checksum_force_native_covers_abstract_change_filter -- --nocapture
```

Results:

```text
cratonvm-native-builtins: 2 passed
cratonvm-vm: 1 passed
```

Suite-runner command:

```powershell
& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -Vm craton -Category others -Jit off `
  -ClassList 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\keycloak-model-realm-stw-20260708-001.tsv' `
  -RunName 'keycloak-liquibase-checksum-status-verify-20260709-003' `
  -Parallel 1 -TimeoutSec 900 `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' `
  -WorkDir 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite' `
  -Exe 'C:\craton\cargo-targets\keycloak-liquibase-checksum-status-20260709-003\release\cratonvm-keycloak-liquibase-checksum-status-20260709-003.exe' `
  -JdkHome 'C:\Program Files\Java\jdk-25'
```

Result:

```text
FAIL 851.630s org.keycloak.testsuite.model.RealmModelTest
```

`results.tsv` records:

```text
=> java.lang.NoSuchMethodError: java/lang/Object.compare(Ljava/lang/Object;Ljava/lang/Object;)I
```

The runner stdout failure stack starts:

```text
java.lang.NoSuchMethodError: java/lang/Object.compare(Ljava/lang/Object;Ljava/lang/Object;)I
liquibase.snapshot.SnapshotGeneratorFactory.getGenerators(SnapshotGeneratorFactory.java:125)
liquibase.snapshot.DatabaseSnapshot.createGeneratorChain(DatabaseSnapshot.java:578)
liquibase.snapshot.DatabaseSnapshot.include(DatabaseSnapshot.java:314)
liquibase.snapshot.DatabaseSnapshot.init(DatabaseSnapshot.java:112)
liquibase.snapshot.JdbcDatabaseSnapshot.<init>(JdbcDatabaseSnapshot.java:37)
liquibase.snapshot.SnapshotGeneratorFactory.createSnapshot(SnapshotGeneratorFactory.java:343)
liquibase.snapshot.SnapshotGeneratorFactory.checkExistence(SnapshotGeneratorFactory.java:260)
liquibase.snapshot.SnapshotGeneratorFactory.has(SnapshotGeneratorFactory.java:220)
org.keycloak.connections.jpa.updater.liquibase.custom.CustomCreateIndexChange.generateStatements(CustomCreateIndexChange.java:82)
```

The stderr log shows the run reached Liquibase update execution and completed
substantial DDL/index work before the comparator failure:

```text
CREATE INDEX PUBLIC.IDX_REALM_ATTR_REALM ON PUBLIC.REALM_ATTRIBUTE(REALM_ID)
NoSuchMethodError method="java/lang/Object.compare(Ljava/lang/Object;Ljava/lang/Object;)I"
  caller="liquibase/snapshot/SnapshotGeneratorFactory.getGenerators(Ljava/lang/Class;Lliquibase/database/Database;)Ljava/util/SortedSet; @pc=70"
NoSuchMethodError method="java/lang/Object.apply(Ljava/lang/Object;)Ljava/lang/Object;"
  caller="liquibase/snapshot/SnapshotGeneratorFactory.getGenerators(Ljava/lang/Class;Lliquibase/database/Database;)Ljava/util/SortedSet; @pc=70"
UPDATE SUMMARY
Run:                         42
Total change sets:          195
```

## Current hypothesis

The active failure is a generic lambda-proxy / functional-interface dispatch
gap exposed by Liquibase's comparator construction in
`SnapshotGeneratorFactory.getGenerators`. The `Object.compare` and
`Object.apply` names suggest a comparator/function lambda body is being invoked
through a receiver or fallback class of `java/lang/Object` instead of through
the lambda proxy's SAM dispatch metadata.

The checksum `AbstractChange$1.include` native shortcut should stay: it removes
a confirmed hot leaf in the earlier status/checksum stack, but the remaining
suite blocker is now the comparator lambda dispatch.

## Next leads

- Build a minimal Java probe for `Comparator.comparing(...).thenComparing(...)`
  or the exact `SnapshotGeneratorFactory.getGenerators` comparator chain under
  CratonVM `--nojit`.
- Instrument `try_lambda_dispatch` for `java/util/Comparator.compare` and
  `java/util/function/Function.apply` call sites that resolve to
  `java/lang/Object`.
- Check whether the receiver-is-lambda-proxy rescue handles this nested
  comparator/function shape or whether `coerce_lambda_args` is losing the
  captured comparator/function receiver.
- Keep the Xerces parser intrinsics and `AbstractChange$1.include` native
  shortcut in place; both are necessary progress but neither closes the current
  suite failure alone.
