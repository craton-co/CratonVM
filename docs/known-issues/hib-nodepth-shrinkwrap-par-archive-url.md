# Hibernate `NoDepthTests` (JPA variants) — ShrinkWrap in-memory `.par` archive has no URL handler

| | |
|---|---|
| **Status** | OPEN (niche — ShrinkWrap in-memory archive URL stream handler unimplemented) |
| **Area** | VM — custom URL stream handler / `ClassLoader.getResource` for ShrinkWrap's in-memory `JavaArchive` |
| **Symptom** | `org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests` JPA variants fail: `RuntimeException: Could not create URL for archive: fetch-depth.par`. |
| **Severity** | low (CratonVM-only; pre-existing — fails identically at baseline `b0aab8f9`; only the 2 JPA variants of 4 tests are affected). |
| **Discovered** | 2026-06-24, triaging the Hibernate suite residuals after the collection-delegation stack-overflow fix (`7b224d8a`). |

## Symptom

`NoDepthTests` has 4 methods. The two non-JPA variants (`testWithMax`,
`testNoMax`) build a `SessionFactory` directly and **pass**. The two JPA variants
(`testWithMaxJpa`, `testNoMaxJpa`) construct an **in-memory** ShrinkWrap archive
and load a persistence unit out of it:

```java
final JavaArchive par = ShrinkWrap.create( JavaArchive.class, "fetch-depth.par" );
par.addClasses( SysModule.class );
par.addAsResource( "units/many2many/fetch-depth.xml", "META-INF/persistence.xml" );
try ( ShrinkWrapClassLoader classLoader = new ShrinkWrapClassLoader( par ) ) {
    …
    createEntityManagerFactory( "fetch-depth", settings );   // → fails here
}
```

These fail with:

```
java.lang.RuntimeException: Could not create URL for archive: fetch-depth.par
```

## Root cause

`ShrinkWrapClassLoader` exposes its in-memory `JavaArchive` to consumers
(Hibernate's persistence-unit scanner needs a `URL` for the archive so it can find
`META-INF/persistence.xml`) by registering a **custom `URLStreamHandler`** for an
in-memory protocol and creating `URL`s against it. CratonVM does not support
constructing a `URL` backed by ShrinkWrap's in-memory handler, so
`ShrinkWrapClassLoader.getResource(...)` / archive-URL creation throws, and the JPA
bootstrap cannot read the persistence descriptor.

This is the same class of gap as other "in-memory / custom URL protocol" issues —
the archive is never on a real filesystem path, so resource enumeration must go
through the application-supplied `URLStreamHandler`.

## Impact

- `NoDepthTests.testWithMaxJpa` / `testNoMaxJpa` (2 of 4; the other 2 pass).
- Any test using ShrinkWrap in-memory archives + `ShrinkWrapClassLoader` for JPA
  persistence-unit discovery.

## Next steps

Support a `URL` backed by an application-registered `URLStreamHandler` (here
ShrinkWrap's in-memory handler) so `ShrinkWrapClassLoader.getResource` and the
archive-URL path resolve. Niche; low priority relative to the other Hibernate
residuals. Verify against the 2 JPA variants of `NoDepthTests`.
