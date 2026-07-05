# Hibernate `NoDepthTests` JPA variants — `PersistenceProvider` ServiceLoader discovery fails through ShrinkWrap's custom classloader (residual of the fixed URL/addURL bug)

| | |
|---|---|
| **Status** | 🔴 OPEN — new residual, surfaced only after the original bug (below) was fixed and merged. |
| **Area** | `ServiceLoader<jakarta.persistence.spi.PersistenceProvider>` discovery through a custom (`ShrinkWrapClassLoader`) classloader |
| **Symptom** | `jakarta.persistence.PersistenceException: No Persistence provider for EntityManager named fetch-depth` |
| **Severity** | low — 2 of 4 test methods in one class. |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |

## Symptom

```
@@FAIL org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests :: jakarta.persistence.PersistenceException: No Persistence provider for EntityManager named fetch-depth
@@FAIL org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests :: jakarta.persistence.PersistenceException: No Persistence provider for EntityManager named fetch-depth
```

(2 failures — the two JPA variants, `testWithMaxJpa`/`testNoMaxJpa`; the two
non-JPA variants pass, matching HotSpot's `found=4 ok=4 failed=0`.)

## Relationship to the already-fixed bug

[hib-nodepth-shrinkwrap-par-archive-url.md](../internal/hibernate-bugs/hib-nodepth-shrinkwrap-par-archive-url.md)
(FIXED, merged via `bf36c942`) documents and fixes the *original* failure for
these exact two test methods:
```
java.lang.RuntimeException: Could not create URL for archive: fetch-depth.par
```
That fix (confirmed merged and an ancestor of the current dev tip) made
`URLClassLoader.addURL`, `findResource(s)`, and `URL.openStream` work
correctly against ShrinkWrap's in-memory `archive:`-scheme classloader, so
`createEntityManagerFactory("fetch-depth", settings)` now gets **past** URL
construction and resource resolution — but fails at the **next** step:
`Persistence.createEntityManagerFactory` can no longer find a matching
`PersistenceProvider`.

This is a **new, deeper layer of the same underlying gap**, not a
regression: the fixed doc only ever claimed the URL/`addURL`/resource-lookup
chain was fixed, and explicitly scoped out ServiceLoader-based
`PersistenceProvider` discovery as out of scope. HotSpot passes both JPA
variants (`found=4 ok=4 failed=0`), confirming this residual is
CratonVM-specific too.

## Hypothesis (not yet verified)

`Persistence.createEntityManagerFactory(name, ...)` internally does
`ServiceLoader.load(PersistenceProvider.class, <classloader>)`, where
`<classloader>` should be the thread's context classloader — in this test,
the `ShrinkWrapClassLoader` (a `URLClassLoader` subclass) set as TCCL around
the `createEntityManagerFactory` call. For `ServiceLoader` to find
Hibernate's `org.hibernate.jpa.HibernatePersistenceProvider`, it needs to
resolve `META-INF/services/jakarta.persistence.spi.PersistenceProvider` via
that classloader's `getResources(...)` — which, per the delegation model,
should fall through to the parent (the real application classloader that
has Hibernate on its classpath) since `ShrinkWrapClassLoader` only holds the
in-memory PAR's own resources.

Likely culprit: the `findResource(s)` fix in the referenced doc only handles
the loader's own recorded custom-handler URLs plus the "global dynamic
classpath" walk; if `ServiceLoader`'s multi-resource enumeration
(`getResources`, plural — collecting **every** provider-config file visible
from this loader, parent included) doesn't correctly delegate to/merge with
the parent loader's real `META-INF/services` entries in CratonVM's
classloader-shimming model, `ServiceLoader` sees zero candidate provider
classes and `createEntityManagerFactory` reports "No Persistence provider".

## Repro

Azure host, harness at `/home/victor/hibpkg/runner`:
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests) 0
```

## Next steps (not yet done)

- Instrument `ServiceLoader.load(PersistenceProvider.class, tccl)` (or the
  underlying `getResources("META-INF/services/...")` call) to see whether
  it returns 0 results or 1+ results that then fail some other provider
  filter.
- Check `URLClassLoader.getResources` / `ClassLoader.getResources`
  delegation-to-parent behavior generally under CratonVM's classloader
  shimming — if this is a general "custom classloader's plural
  getResources doesn't merge parent results" gap, it likely affects more
  than just this one Hibernate test.
