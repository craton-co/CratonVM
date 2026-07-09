# RealmModelTest no-JIT timeout in Liquibase/Xerces XML schema parsing

Status: open

Date observed: 2026-07-08, after fixing the protobuf metadata cache
configuration residual and its follow-ons.

## Summary

`RealmModelTest` under CratonVM `--nojit` now gets past the earlier
Infinispan/protobuf, FFM, StampedLock, and Liquibase pipeline-order failures.
It still exceeds the 900s suite-runner watchdog while loading Keycloak's
Liquibase changelog XML and validating the Liquibase `dbchangelog-3.2.xsd`.

This is a CratonVM no-JIT throughput residual, not the old protobuf metadata
configuration failure. HotSpot `-Xint` passed the same class/list in 142.649s.

## Evidence

Latest CratonVM run:

```powershell
& 'C:\craton\CratonVM\apps\keycloak-suite-runner\run-keycloak-suite.ps1' `
  -Vm craton -Category others -Jit off `
  -ClassList 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\keycloak-model-realm-stw-20260708-001.tsv' `
  -RunName 'keycloak-protobuf-cmstateset-verify-20260708-001' `
  -Parallel 1 -TimeoutSec 900 `
  -KeycloakRoot 'C:\craton\CratonVM\apps\keycloak' `
  -WorkDir 'C:\craton\CratonVM\apps\keycloak-suite-runner\.suite' `
  -Exe 'C:\craton\cargo-targets\keycloak-protobuf-metadata-20260708-001\release\cratonvm-keycloak-protobuf-metadata-20260708-001.exe'
```

Result:

```text
HANG 900.078s org.keycloak.testsuite.model.RealmModelTest
```

The stderr tail reached Liquibase checksum generation for schema changes and
the old signatures were absent:

- no `ISPN000436`
- no `SymbolLookup`
- no `NoSuchElementException`
- no `IllegalMonitorStateException`
- no `ISPN000659`
- no `ConcurrentHashMap does not permit null keys`

A manual watchdog run with stack dumps:

```text
C:\craton\CratonVM\apps\keycloak-suite-runner\.suite\results\keycloak-protobuf-manual-watchdog-20260708-004\stderr.log
```

At 600s, the main thread was parsing an included changelog and resolving
`dbchangelog-3.2.xsd`:

```text
liquibase.parser.core.xml.XMLChangeLogSAXParser.parseToNode
com.sun.org.apache.xerces.internal.jaxp.SAXParserImpl$JAXPSAXParser.parse
com.sun.org.apache.xerces.internal.impl.XMLNSDocumentScannerImpl.scanStartElement
com.sun.org.apache.xerces.internal.impl.xs.XMLSchemaValidator.handleStartElement
com.sun.org.apache.xerces.internal.impl.xs.XMLSchemaLoader.loadSchema
com.sun.org.apache.xerces.internal.impl.xs.traversers.XSDHandler.parseSchema
com.sun.org.apache.xerces.internal.impl.xs.opti.SchemaDOMParser.parse
com.sun.org.apache.xerces.internal.impl.XMLNSDocumentScannerImpl.scanAttribute
com.sun.org.apache.xerces.internal.impl.XMLEntityScanner.scanQName
com.sun.org.apache.xerces.internal.impl.XMLEntityScanner.checkLimit
jdk.xml.internal.XMLLimitAnalyzer.addValue
```

The specific file in progress just before the dump was:

```text
META-INF/jpa-changelog-1.1.0.Final.xml
systemId='http://www.liquibase.org/xml/ns/dbchangelog/dbchangelog-3.2.xsd'
```

## Current hypothesis

This looks like the same broad Xerces interpreter-throughput class documented
for earlier Hibernate XSD work, but with a different current stack. The latest
branch already added native fast paths for `CMStateSet.hashCode`,
`CMStateSet.equals`, and `CMStateSet.isSameSet`, which moved the Keycloak run
past the prior XSD DFA hotspot. The remaining watchdog point is now the XML
scanner/schema-loading path (`scanQName` / `checkLimit` /
`XMLLimitAnalyzer.addValue`).

Do not close this as the protobuf metadata bug: that bug is fixed and archived
separately. This note owns the remaining no-JIT Liquibase/Xerces timeout.

## Next leads

- Profile or instrument the no-JIT XML scanner path rather than adding a
  semantics-changing no-op for `XMLLimitAnalyzer.addValue`; it enforces JDK XML
  security limits.
- Candidate low-risk areas to measure first: `XMLChar.isNameStart`,
  `XMLChar.isName`, `SymbolTable.addSymbol`, and `XMLLimitAnalyzer.addValue`
  with accurate field/array semantics.
- Compare with HotSpot `-Xint` on the same class/list to keep the target at the
  known passing baseline of about 142.649s.
