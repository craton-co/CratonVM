# RealmModelTest no-JIT timeout in Liquibase checksum/status serialization

Status: open

Date observed: 2026-07-09, after fixing the first Liquibase/Xerces XML parse
timeout.

## Summary

`RealmModelTest` under CratonVM `--nojit` now gets past the earlier
Liquibase/Xerces XML schema parsing hotspot. The old stack in
`XMLEntityScanner.scanQName -> checkLimit -> XMLLimitAnalyzer.addValue` no
longer appears in the representative watchdog evidence.

The first open residual was the Liquibase checksum/status serializer path:
`ChangeSet.generateCheckSum -> AbstractChange.generateCheckSum ->
StringChangeLogSerializer.serializeObject -> AbstractChange$1.include`. A
native override now covers `AbstractChange$1.include` and directly scans the
excluded-field array instead of interpreting the per-field stream/`anyMatch`
predicate.

That mitigation is not sufficient to retire the issue. With the override in
place, the class still exceeds the 900-second suite-runner watchdog. A
300-second direct stack-dump run samples the main thread in
`LiquibaseJpaUpdaterProvider.validate -> getLiquibaseUnrunChangeSets`, while
Liquibase is parsing included changelog XML and Xerces is traversing schema
grammar attributes.

HotSpot `-Xint` previously passed the same class/list in 142.649s, so this
remains a CratonVM no-JIT throughput residual rather than a Keycloak functional
failure.

## Evidence

The unique CratonVM binary used for the latest validation was:

```text
C:\craton\cargo-targets\keycloak-liquibase-checksum-status-20260709-002\release\cratonvm-keycloak-liquibase-checksum-status-20260709-002.exe
```

Focused native/dispatch tests:

```powershell
$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-devtests-20260709-006'
cargo test -p cratonvm-native-builtins liquibase_checksum -- --nocapture

$env:CARGO_TARGET_DIR='C:\craton\cargo-targets\keycloak-liquibase-vmtests-20260709-006'
cargo test -p cratonvm-vm liquibase_checksum_force_native_covers_abstract_change_filter -- --nocapture
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
  -RunName 'keycloak-liquibase-checksum-status-verify-20260709-002' `
  -Parallel 1 -TimeoutSec 900 `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' `
  -WorkDir 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite' `
  -Exe 'C:\craton\cargo-targets\keycloak-liquibase-checksum-status-20260709-002\release\cratonvm-keycloak-liquibase-checksum-status-20260709-002.exe' `
  -JdkHome 'C:\Program Files\Java\jdk-25'
```

Result:

```text
HANG 900.113s org.keycloak.testsuite.model.RealmModelTest
```

The direct stack-dump run used the same unique binary with
`--stack-dump-on-timeout 300 --Xmx 2g --nojit` from
`apps/keycloak/testsuite/model`, with the pathing jar
`apps/keycloak-suite-runner/.suite/pathing-jars/testsuite_model-122e55ba66167a16.jar`.
The run directory was
`apps/keycloak-suite-runner/.suite/results/keycloak-liquibase-checksum-status-direct-stack-20260709-003`.
It exited after the watchdog dump with process status `-1073740791`.

Representative main-thread stack:

```text
org.keycloak.connections.jpa.updater.liquibase.LiquibaseJpaUpdaterProvider.validate
org.keycloak.connections.jpa.updater.liquibase.LiquibaseJpaUpdaterProvider.validateSynch
org.keycloak.connections.jpa.updater.liquibase.LiquibaseJpaUpdaterProvider.validateChangeSet
org.keycloak.connections.jpa.updater.liquibase.LiquibaseJpaUpdaterProvider.getLiquibaseUnrunChangeSets
liquibase.Liquibase.listUnrunChangeSets
liquibase.Liquibase.getDatabaseChangeLog
liquibase.parser.core.xml.AbstractChangeLogParser.parse
liquibase.changelog.DatabaseChangeLog.handleInclude
liquibase.parser.core.xml.XMLChangeLogSAXParser.parseToNode
com.sun.org.apache.xerces.internal.jaxp.SAXParserImpl$JAXPSAXParser.parse
com.sun.org.apache.xerces.internal.parsers.XMLParser.parse
com.sun.org.apache.xerces.internal.parsers.XML11Configuration.parse
com.sun.org.apache.xerces.internal.impl.XMLDocumentFragmentScannerImpl.scanDocument
com.sun.org.apache.xerces.internal.impl.XMLNSDocumentScannerImpl.scanStartElement
com.sun.org.apache.xerces.internal.impl.xs.XMLSchemaValidator.findSchemaGrammar
com.sun.org.apache.xerces.internal.impl.xs.XMLSchemaLoader.loadSchema
com.sun.org.apache.xerces.internal.impl.xs.traversers.XSDHandler.parseSchema
com.sun.org.apache.xerces.internal.impl.xs.traversers.XSDHandler.traverseSchemas
com.sun.org.apache.xerces.internal.impl.xs.traversers.XSDComplexTypeTraverser.traverseGlobal
com.sun.org.apache.xerces.internal.impl.xs.traversers.XSDAbstractTraverser.traverseAttrsAndAttrGrps
com.sun.org.apache.xerces.internal.impl.xs.XSAttributeGroupDecl.getAttributeUseNoProhibited
```

## Current hypothesis

This remains a Liquibase no-JIT throughput family rather than a single fixed
method. The checksum/status filter was one hot layer and now has a direct native
override. The latest sampled residual is back in changelog XML parsing, but not
the already-fixed `scanQName/checkLimit/XMLLimitAnalyzer.addValue` path. The
current evidence points at Xerces schema grammar loading/traversal during
Liquibase validation, especially `XMLSchemaValidator.findSchemaGrammar`,
`XMLSchemaLoader.loadSchema`, and XSD traverser attribute-group handling.

## Next leads

- Compare the 300-second schema traversal stack against HotSpot `-Xint` method
  counts for the same changelog include.
- Instrument Xerces XSD traversal methods under CratonVM `--nojit`, starting at
  `XMLSchemaValidator.findSchemaGrammar`, `XMLSchemaLoader.loadSchema`,
  `XSDHandler.parseSchema`, `XSDAbstractTraverser.traverseAttrsAndAttrGrps`, and
  `XSAttributeGroupDecl.getAttributeUseNoProhibited`.
- Keep the checksum `AbstractChange$1.include` native override in place; it is a
  proven shortcut for the prior status/checksum stack, but it does not close the
  current timeout alone.
