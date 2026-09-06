# PublicSuffixList fails to cast to itself in every Spring test that forks/modifies the classpath

## Status

**OPEN.** Symptom confirmed reproducible and GC-independent across all three
collectors, and (2026-09-05) confirmed independently in **Spring Framework**
as well as Spring Boot — same third-party class, two unrelated forked/child
`ClassLoader` mechanisms, different codebases. Root cause narrowed to the
general shape "a forked classloader whose parent delegation chain excludes
the loader that already resolved the class" — see
`CompileWithForkedClassLoaderClassLoader`'s exact mechanism below — but the
specific CratonVM classloading gap that lets the class get loaded under the
"wrong" loader in the first place is still not pinned to a source line.

## The symptom

Four classes, identical stack frame, full 3-GC Spring Boot suite run
(2026-09-04/05, `full_{generational,g1,zgc}_mysession_20260904_041104`):

```
java.lang.ClassCastException: class org.apache.hc.client5.http.psl.PublicSuffixList
  cannot be cast to class org.apache.hc.client5.http.psl.PublicSuffixList
	at org.apache.hc.client5.http.psl.PublicSuffixMatcher.<init>(PublicSuffixMatcher.java:88)
```

- `org.springframework.boot.restclient.autoconfigure.RestClientObservationAutoConfigurationWithoutMetricsTests`
- `org.springframework.boot.restclient.autoconfigure.RestTemplateObservationAutoConfigurationWithoutMetricsTests`
- `org.springframework.boot.security.autoconfigure.web.servlet.SecurityFilterAutoConfigurationEarlyInitializationTests`
- `org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests`

`FAIL` on Generational, G1, and ZGC alike (one of the four showed `CRASH` on
Generational specifically, but that was the now-fixed generational-heap
`is_object_address` bug masking the identical underlying `FAIL` — G1 and ZGC
on the same run already showed the real failure cleanly for that class).

A class failing to cast to itself, by name, is the textbook signature of the
same class having been loaded by two different classloaders — `Class`
identity in the JVM is `(name, defining loader)`, not name alone.

## Two more plausible mechanisms checked and ruled out

This project has a **prior, unrelated** bug with the exact same misleading
message shape — `bug-h2-testmultithread-mvstore-writer-object-identity-20260816.md`'s
retired `"cannot be cast to class org.h2.util.CloseWatcher"` — but that one
was a GC-relocation stale-address reuse (a vacated address getting recycled
for an unrelated object under ZGC compaction), not real classloader
duplication. Before assuming this is the same *kind* of bug under a new
name, both alternate mechanisms from that precedent were checked here and
ruled out:

- **Not GC-relocation-dependent.** The H2 bug was compaction-correlated
  (2/6 with ZGC relocation on, 0/14 with it off). This failure is identical
  on Generational, G1, and ZGC — a GC-timing-dependent stale pointer would
  not produce the same deterministic failure on three collectors with very
  different compaction/relocation behavior.
- **Not a duplicate jar on the classpath.** `spring-boot-restclient`'s own
  `cratonvm-test-cp.txt` carries exactly one `httpclient5-5.6.3.jar` and one
  `httpcore5-5.4.3.jar` — no second copy of `PublicSuffixList` anywhere on
  the flat classpath to shadow the first (unlike the Netty BouncyCastle jar
  ordering bug this session already fixed).

## What all four classes have in common

All four import and use Spring Boot's own test classloader-forking
infrastructure:

- `RestClientObservationAutoConfigurationWithoutMetricsTests` and
  `RestTemplateObservationAutoConfigurationWithoutMetricsTests`:
  `org.springframework.boot.testsupport.classpath.ClassPathExclusions`
- `SecurityFilterAutoConfigurationEarlyInitializationTests`:
  `org.springframework.boot.testsupport.classpath.ClassPathExclusions`
- `MultipartAutoConfigurationTests`:
  `org.springframework.boot.testsupport.classpath.ForkedClassPath`

Both annotations drive Spring Boot's `ModifiedClassPathExtension` / forked
test-classloader machinery — the test body runs under a **child classloader
built with a deliberately modified classpath** (excluding or replacing
specific jars), rather than the harness's normal application classloader.
No other failing or passing class sampled in this run's FAIL set uses this
mechanism.

**Working hypothesis (not yet confirmed at the CratonVM source level):**
something in CratonVM's classloader delegation does not correctly keep
`PublicSuffixList` scoped to one loader across the parent/child classloader
boundary Spring Boot's fork introduces — most likely the child (forked)
loader ends up loading its own copy of a class that should have delegated to
the parent, or vice versa. This has not been traced to a specific
classloading code path; it is inferred from every failing class sharing this
one test-infrastructure feature and no failing or passing class in this run
lacking it having the same crash.

## What is NOT claimed

- The exact CratonVM classloader code responsible has not been identified.
- Whether *only* `PublicSuffixList` is affected, or any class loaded early
  enough in a forked-classpath test would show the same symptom, is
  untested — `PublicSuffixMatcher.<init>` is simply the first place in this
  run's failures that happened to construct one.
- Not cross-checked against HotSpot on this same harness — plausible but
  unconfirmed that HotSpot's classloader delegation handles Spring Boot's
  forked-classpath extension differently (correctly) here.

## Repro

```bash
cd apps/spring-boot-suite-runner
CV_BIN=<binary> ./run-spring-boot-suite.sh -Category all \
  -ClassList <(printf 'org.springframework.boot.servlet.autoconfigure.MultipartAutoConfigurationTests\n')
grep -c 'PublicSuffixList$' <output>/logs/*.out.log
```

Reproduces on all three collectors; not GC-specific, so a single-collector
run is sufficient to confirm.

## Cross-project evidence, 2026-09-05: the identical failure, a different Spring project, a different fork mechanism

The **Spring Framework** suite (`apps/spring-suite-runner`, not Spring Boot —
a separate codebase, separate test infrastructure), 2026-09-05 rerun, shows
the **exact same class** failing to cast to itself, in **all three** GC arms
(gen `out/jit-real-custom-20260905-182523`, G1 `...-182527`, ZGC
`...-182530`):

```
FAILCAUSE org.springframework.web.service.registry.HttpServiceProxyRegistrationAotProcessorTests :: processHttpServiceProxyWhenSameClientTypeInDifferentGroups() :: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'httpServiceProxyRegistry': class org.apache.hc.client5.http.psl.PublicSuffixList cannot be cast to class org.apache.hc.client5.http.psl.PublicSuffixList
FAILCAUSE org.springframework.web.service.registry.ImportHttpServiceRegistrarTests :: basicListingWithAot() :: org.springframework.beans.factory.BeanCreationException: Error creating bean with name 'httpServiceProxyRegistry': class org.apache.hc.client5.http.psl.PublicSuffixList cannot be cast to class org.apache.hc.client5.http.psl.PublicSuffixList
```

Both classes' sources
(`spring-web/src/test/java/org/springframework/web/service/registry/HttpServiceProxyRegistrationAotProcessorTests.java`
and `ImportHttpServiceRegistrarTests.java` in `apps/spring-framework`) import
and use `org.springframework.core.test.tools.CompileWithForkedClassLoader` /
`TestCompiler` / `Compiled`, applying `@CompileWithForkedClassLoader` to the
specific test methods above. This is **Spring Framework's own** forked/child
classloader mechanism for AOT-generated test code — architecturally the same
shape as Spring Boot's `ClassPathExclusions`/`ForkedClassPath`
(`ModifiedClassPathExtension`), but a completely independent implementation in
a different module. No other mechanism is shared between the two projects'
test infrastructure here; this is not the same jar, the same test runner, or
the same annotation class — it is the same *pattern* (compile/load test code
under a deliberately non-parent-delegating child `ClassLoader`), hit twice,
independently, by two different Spring codebases, landing on the exact same
third-party class.

This **broadens** the working hypothesis from "something in Spring Boot's
`ModifiedClassPathExtension` handling" to "something in how CratonVM handles
**any** classloader that intentionally skips/narrows its parent's delegation
for already-resolvable classes" — i.e. the shared ingredient is not a specific
test-runner API, but the *shape* of a forked classloader whose parent chain
does not include the classloader that most likely already loaded the class in
question.

### The mechanism, read from Spring Framework's own forked-loader source

`CompileWithForkedClassLoaderClassLoader`
(`spring-core-test/src/main/java/org/springframework/core/test/tools/CompileWithForkedClassLoaderClassLoader.java`)
is small enough to read in full, and it pins down exactly what "forked" means
here:

```java
public CompileWithForkedClassLoaderClassLoader(ClassLoader testClassLoader) {
    super(testClassLoader.getParent());   // <-- parent is the ORIGINAL
                                           //     loader's PARENT, skipping it
    this.testClassLoader = testClassLoader;
}

@Override
public Class<?> loadClass(String name) throws ClassNotFoundException {
    if (name.startsWith("org.junit") || name.startsWith("org.testng")) {
        return Class.forName(name, false, this.testClassLoader);
    }
    return super.loadClass(name);   // standard ClassLoader.loadClass:
                                     // checks already-loaded, then asks
                                     // the PARENT (testClassLoader's
                                     // parent, NOT testClassLoader itself)
}

@Override
protected Class<?> findClass(String name) throws ClassNotFoundException {
    byte[] bytes = findClassBytes(name);   // falls back to reading bytes via
                                            // testClassLoader.getResourceAsStream(...)
    return (bytes != null ? defineClass(name, bytes, 0, bytes.length, null)
                          : super.findClass(name));
}
```

For a non-`org.junit`/`org.testng` class like `PublicSuffixList`: `loadClass`
delegates to the JDK's standard `ClassLoader.loadClass`, whose delegation
chain is `[this forked loader] -> testClassLoader.getParent()` — **the
original test classloader itself is deliberately excluded from the chain.**
If `testClassLoader` had *already* loaded `PublicSuffixList` (e.g. eagerly, or
via some earlier code path in the same JVM) *before* the forked-loader test
runs, and the parent classloader above `testClassLoader` does not already
have that exact `Class` object cached, then `findClass` runs: it reads
`PublicSuffixList`'s bytes via `testClassLoader.getResourceAsStream(...)` and
**defines a brand-new `Class` object for the same name under this forked
loader** — genuinely two distinct `Class` objects named
`org.apache.hc.client5.http.psl.PublicSuffixList`, exactly reproducing the
observed `ClassCastException`. This is deliberate, working-as-designed
behavior for a class the forked loader is *supposed* to isolate (that's the
point of `@CompileWithForkedClassLoader` at all) — it goes wrong only if
`testClassLoader` was not supposed to have `PublicSuffixList` loaded yet at
the point the forked loader starts resolving it, and something loads it there
anyway.

**This still stops short of a pinned CratonVM source line** (per this doc's
original scope) — confirming *why* `PublicSuffixList` ends up loaded under
`testClassLoader` before/independent of the forked loader's own resolution
(as opposed to only under the forked loader, which is what a correct run
would produce) needs instrumentation of CratonVM's own class-loading/resolution
eagerness that this session did not have time to add. But the mechanism above
is exact, not inferred, and is now confirmed identical in kind (not just
similar) across two independent Spring codebases and three GC collectors —
strong evidence this is a general CratonVM classloader-delegation gap under
"forked, non-parent-delegating loader" test shapes, not a Spring-Boot-specific
or PublicSuffixList-specific quirk.
