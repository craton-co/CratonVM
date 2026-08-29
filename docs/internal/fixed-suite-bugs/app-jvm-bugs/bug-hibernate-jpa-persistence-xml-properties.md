# Hibernate smoke — JDBC URL not applied from `persistence.xml` (HIB-1)

## Status
**OPEN** on `target/release/cratonvm.exe` (2026-06-05 apps suite).

A fix exists on branch `fix/hibernate-persistence-xml` (`Properties` side-table → CHM mirror) but is **not merged** into the binary used for this run.

## Severity
**HIGH** — JPA bootstrap fails; no `EntityManagerFactory` for XML-configured deployments.

## App / suite
- **Fixture:** `.smoke-cache/hibernate-ri10/fixture/` + Maven jars (Hibernate 6.5.2.Final)
- **Main:** `HibernateSmoke`
- **Harness:** `test-infra/run-all-apps-suites.sh`
- **Log:** `test-infra/suite-results/apps-all-20260605-170945/hibernate-smoke-cratonvm.log`

## Symptom

```
WARN HHH000181: No appropriate connection provider encountered, assuming application will be supplying connections
WARN HHH000342: Could not obtain connection to query metadata
    java.lang.UnsupportedOperationException: The application must supply JDBC connections
…
ServiceException: Unable to create requested service [JdbcEnvironment]
Caused by: Unable to determine Dialect without JDBC metadata
  (please set 'jakarta.persistence.jdbc.url' …)
```

- **rc:** 1 · **wall:** 3.5 s · no `HIB_SMOKE_OK`

Log also shows `%s` placeholders in Hibernate log lines (see [HIB-3](bug-hibernate-log-format-placeholder.md)) and duplicate PU name warnings ([HIB-2](bug-hibernate-duplicate-persistence-unit-scan.md)) — secondary noise.

## HotSpot behavior

Same fixture and `../../../../apps/META-INF/persistence.xml`:

```xml
<property name="jakarta.persistence.jdbc.url" value="jdbc:h2:mem:smoke"/>
<property name="jakarta.persistence.jdbc.driver" value="org.h2.Driver"/>
<property name="hibernate.dialect" value="org.hibernate.dialect.H2Dialect"/>
```

Prints `HIB_SMOKE_OK text=hello`, rc=0, ~3–4 s.

## CratonVM behavior

XML is present on classpath; Hibernate parses the PU name but **JDBC URL / driver / dialect never reach** `EntityManagerFactoryBuilderImpl` merged settings. Programmatic `Configuration.setProperty` works; **XML-derived `Properties` do not survive `HashMap.putAll(pu.getProperties())`.**

## Root cause (CONFIRMED on fix branch)

CratonVM `java.util.Properties` (`native-builtins/src/properties_sidetable.rs`):

- Writes go to a Rust **side-table** keyed by object identity.
- Side-table entries were **not mirrored** into the real JDK `ConcurrentHashMap` backing.
- `HashMap.putAll` / copy constructors enumerate via the CHM → **0 entries copied**.

```
configValues.putAll(persistenceUnit.getProperties())  // empty → no JDBC URL
```

Minimal repro (no Hibernate): `Properties` with 5 `put`s reports `size()==5` but `new HashMap<>(p).size()==0`.

## Fix (on branch, not in this binary)

Mirror every `put`/`setProperty`/`remove`/`clear` into the CHM; fix `keys()`/`elements()` empty enumerations. Verified on `fix/hibernate-persistence-xml` with `PropProbe`, `RegProbe`, and partial `HibernateSmoke` advance to JDBC connect.

## Reproduce

```bash
bash test-infra/run-all-apps-suites.sh
# or
bash test-infra/run-three-apps-suite.sh
```

Requires `.smoke-cache/hibernate-ri10/` (see suite jar downloader in `run-three-apps-suite.sh`).

## Impact

Blocks JPA apps using `persistence.xml` (WildFly, Spring Boot, Quarkus, this smoke).

## Related

- [bug-hibernate-duplicate-persistence-unit-scan.md](bug-hibernate-duplicate-persistence-unit-scan.md) (HIB-2)
- [bug-hibernate-log-format-placeholder.md](bug-hibernate-log-format-placeholder.md) (HIB-3)
- [apps/CRATONVM_CRASHES.md](../../apps/CRATONVM_CRASHES.md)
