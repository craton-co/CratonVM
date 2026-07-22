# Hibernate `NoDepthTests` JPA variants — `PersistenceProvider` ServiceLoader discovery residual (RESOLVED)

| | |
|---|---|
| **Status** | ✅ FIXED on dev — verified resolved 2026-07-05 in branch `fix/hib-nodepth-persistenceprovider-serviceloader-20260705`. No source change was needed; the residual no longer reproduces on current dev. |
| **Area** | `ServiceLoader<jakarta.persistence.spi.PersistenceProvider>` discovery through a custom (`ShrinkWrapClassLoader`) classloader |
| **Symptom (when open)** | `jakarta.persistence.PersistenceException: No Persistence provider for EntityManager named fetch-depth` |
| **Originally discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |
| **Verified fixed** | 2026-07-05, Windows box, dev `04a832b1` (worktree branched from dev at this commit). |

## Original symptom

```
@@FAIL org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests :: jakarta.persistence.PersistenceException: No Persistence provider for EntityManager named fetch-depth
@@FAIL org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests :: jakarta.persistence.PersistenceException: No Persistence provider for EntityManager named fetch-depth
```

(2 of the class's 4 methods — the JPA variants `testWithMaxJpa`/`testNoMaxJpa` — were reported
failing; the 2 non-JPA variants always passed.)

## Relationship to the already-fixed parent bug

[hib-nodepth-shrinkwrap-par-archive-url.md](hib-nodepth-shrinkwrap-par-archive-url.md) (FIXED,
merged via `bf36c942`) fixed the *original* failure for these same two test methods
(`RuntimeException: Could not create URL for archive: fetch-depth.par`) by teaching
`URLClassLoader.addURL`/`findResource(s)`/`URL.openStream` to work against ShrinkWrap's
in-memory `archive:`-scheme classloader. That fix explicitly scoped out ServiceLoader-based
`PersistenceProvider` discovery, and a follow-up triage run at dev `49aaf713` observed the two
JPA variants failing one layer deeper, with `createEntityManagerFactory("fetch-depth", ...)`
getting past URL/resource resolution but then failing to find a `PersistenceProvider`.

## Verification (2026-07-05)

Built a fresh `cratonvm.exe` in a worktree branched from dev @ `04a832b1` (65 commits ahead of
the `49aaf713` discovery point) and ran the documented repro directly against
`org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests`, both with and without the original
repro's `CRATONVM_JIT_OSR=1` flag, 3 runs total:

```
@@RESULT 0 org.hibernate.orm.test.mapping.fetch.depth.NoDepthTests found=4 started=4 ok=4 failed=0 aborted=0 skipped=0 ms=...
```

All 3 runs: **4/4 pass**, matching HotSpot's `found=4 ok=4 failed=0`. `CRATONVM_DIAG_SERVICELOADER=1`
tracing confirms `ServiceLoader.load(PersistenceProvider.class, loader)` now correctly resolves
exactly one provider:

```
[SL-DBG] ServiceLoader service=jakarta.persistence.spi.PersistenceProvider loader_delegation=false descriptors=2 providers=1 (["org.hibernate.jpa.HibernatePersistenceProvider"])
```

## Root cause (not conclusively bisected)

No source fix was made in this session — the residual was already gone on dev. The most likely
fixing commit, based on a `git log` scan of `native-builtins/src/{classloader,service_loader}.rs`
between `49aaf713` and `04a832b1` (65 commits), is `1416534c` ("Fix Keycloak FacadeClassLoader
resource probes"), which hardened `build_custom_handler_url_list` / `merge_enum_with_list` — the
exact code path `URLClassLoader.findResources` (`ucl_find_resources`) uses to merge a custom
classloader's own recorded handler URLs with the flat-classpath scan. That is the same code path
`ServiceLoader`'s `discover_providers` (`service_loader.rs`) and Hibernate's own
`ClassLoaderService.locateResources("META-INF/persistence.xml")` walk depend on for a
`ShrinkWrapClassLoader`-as-TCCL bootstrap. This was not confirmed by bisection (would require an
extra ~10 min cold build at the old commit); given the low severity (2 of 4 methods in one class)
and clean 3/3 reproduction of the fix, a full bisect was judged not worth the build cost.

## Takeaway

The original residual doc's hypothesis (a `getResources`/`findResources` parent-delegation gap
for custom classloaders) pointed at the right general area of code — it was independently
hardened by unrelated Keycloak work shortly after this doc was written, closing the gap before
any dedicated fix was needed here.
