# Hibernate — duplicate persistence-unit scan / classpath enumeration

## Status
**OPEN** on `target-bench` (2026-06-05).

## Severity
**MEDIUM** — may contribute to JPA bootstrap failure or cause subtle duplicate-PU errors.

## App / suite
- **Suite:** RI.10 `HibernateSmoke`
- **Logs:** `test-infra/suite-results/apps-three-20260605-141545/hibernate-smoke-cratonvm-run1.err`

## Symptom

```
HHH015018: Encountered multiple persistence-unit stanzas defining same name [%s]; persistence-unit names must be unique
```

Message appears **twice** at startup. Placeholder `%s` not substituted (logging bug — see [bug-hibernate-log-format-placeholder.md](bug-hibernate-log-format-placeholder.md)).

## HotSpot behavior

No duplicate-PU warning for the same fixture. Only one `../../../../apps/META-INF/persistence.xml` exists on classpath (`fixture/META-INF/persistence.xml` with single PU `smoke`).

Verified: no `persistence.xml` inside `hibernate-core-6.5.2.Final.jar`.

## CratonVM behavior

Hibernate’s `PersistenceXmlParser` believes it saw **multiple stanzas** named `smoke`. Possible explanations:

1. **Classpath scanning** lists the same resource URL twice (duplicate classpath entries or broken `URLClassLoader` enumeration)
2. **Jar scanning** reads `../../../../apps/META-INF/persistence.xml` from fixture **and** incorrectly from another entry
3. **XML parser** double-invokes element handlers for one file

## Root cause (suspected)

Classpath / resource enumeration or XML parse event duplication in CratonVM.

**Suspect areas:** `ClassLoader.getResources`, jar file iteration, StAX/SAX callback semantics.

## Impact

- May prevent valid PU from being selected (pairs with [bug-hibernate-jpa-persistence-xml-properties.md](bug-hibernate-jpa-persistence-xml-properties.md))
- Real apps with single PU could fail mysteriously on CratonVM

## Reproduce

```bash
bash test-infra/run-three-apps-suite.sh
grep HHH015018 test-infra/suite-results/apps-three-*/hibernate-smoke-cratonvm-run1.err
```

Debug probe: enumerate all `../../../../apps/META-INF/persistence.xml` URLs on classpath under both VMs.

## What to fix

1. Compare `ClassLoader.getResources("META-INF/persistence.xml")` URL list CratonVM vs HotSpot.
2. Deduplicate if same URL returned twice, or fix root cause of double scan.
3. Re-run `HibernateSmoke`; warning should disappear.

## Related

- [bug-hibernate-jpa-persistence-xml-properties.md](bug-hibernate-jpa-persistence-xml-properties.md)
