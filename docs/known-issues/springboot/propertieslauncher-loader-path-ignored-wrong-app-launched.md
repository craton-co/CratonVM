# `PropertiesLauncher` appears to ignore `loader.path`, launches the wrong app

**Status: OPEN — found 2026-07-17, hypothesis only (root cause not pinned to source)**

**Update 2026-07-19:** the classpath URL enumeration fix in
[`spring-boot-loader-classpath-url-enumeration-empty-cluster-FIXED.md`](../../internal/springboot/spring-boot-loader-classpath-url-enumeration-empty-cluster-FIXED.md)
(better `URL`/classpath resolution generally — see its `extract_url_path`
and `ucl_try_define_local_class` changes) fixed 8 of the 11 tests originally
listed below as a side effect: `testUserSpecifiedJarPath`,
`testUserSpecifiedWildcardPath`, `testUserSpecifiedRootOfJarPathWithDot`,
`testUserSpecifiedDirectoryContainingJarFileWithNestedArchives`,
`testUserSpecifiedJarPathWithDot`, `testUserSpecifiedJarFileWithNestedArchives`,
`testUserSpecifiedRootOfJarPath`, `testUserSpecifiedRootOfJarPathWithDotAndJarPrefix`.
Re-measured full-class run (`SbRunner` against a fresh build, 2026-07-19):
28/32 tests pass, 4 fail — 3 from the original 11
(`testUserSpecifiedNestedJarPath`, `testUserSpecifiedClassLoader`,
`classPathWithoutLoaderPathDefaultsToJarLauncherIncludes`) plus one further
failure, `testUserSpecifiedClassPathOrder`, not present in the original
2026-07-17 catalogue below (current `apps/spring-boot` checkout is Spring
Boot 4.1.0-SNAPSHOT and its fixture set moves; not yet reconciled against
the original 11 by name). Root-cause hypothesis below is otherwise
unchanged — not re-investigated this pass.

## Symptom

11 of `PropertiesLauncherTests`' 14 failures share one signature: the test
sets `System.setProperty("loader.path", ...)` to point at a fixture jar whose
`main()` prints `"Hello World"`, launches via `PropertiesLauncher`, and
polls (`Awaitility`) for that output — but consistently observes
`"Hello Other World"` instead, the output of a *different* fixture jar the
test never pointed `loader.path` at:

```
=> org.awaitility.core.ConditionTimeoutException: Lambda expression in org.springframework.boot.loader.launch.PropertiesLauncherTests expected a string containing "Hello World" but was "Hello Other World
" within 5 seconds.
   org.awaitility.core.ConditionAwaiter.await(ConditionAwaiter.java:167)
   org.springframework.boot.loader.launch.PropertiesLauncherTests.waitFor(PropertiesLauncherTests.java:423)
   org.springframework.boot.loader.launch.PropertiesLauncherTests.testUserSpecifiedJarPath(PropertiesLauncherTests.java:173)
```

The remaining tests in this same 11 show the equivalent symptom in a
different assertion shape — a `ClassNotFoundException: demo.Application` (the
intended app class was never on the classpath the launcher actually used) or
`"Expecting elements: [<expected classpath URL>] to be exactly 1 times 1"`
(the expected, `loader.path`-derived classpath entry is simply absent).

Full log:
`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260717/shard2/logs/loader_spring-boot-loader.org.springframework.boot.loader.launch.PropertiesLauncherTests.out.log`

## Not the same as the classpath-URL-enumeration cluster

3 of `PropertiesLauncherTests`' 14 failures (`jarFilesPresentInBootInfLibsAndNotInClasspathIndexShouldBeAddedAfterBootInfClasses`,
`explodedJarShouldPreserveClasspathOrderWhenIndexPresent`,
`customClassLoaderAndExplodedJarAndShouldPreserveClasspathOrderWhenIndexPresent`)
show the `actual: []` empty-classpath-URL-set shape documented in
`spring-boot-loader-classpath-url-enumeration-empty-cluster.md` and are
tracked there instead. This doc covers only the other 11, whose symptom is
qualitatively different: they don't get an *empty* result, they get the
*wrong, but non-empty and internally consistent*, result — as if `loader.path`
were silently not applied and the launcher fell back to resolving the app
from wherever its own class was loaded.

## Root cause — hypothesis, unconfirmed

`grep -rn "PropertiesLauncher" native-builtins/src` finds exactly one
registration touching a `PropertiesLauncher`-named class
(`native-builtins/src/phases_late.rs:22170`), and it targets
`org/springframework/boot/loader/PropertiesLauncher` — the **Spring Boot 2**
package (no `.launch.` segment). These tests exercise the **Spring Boot 3**
class, `org.springframework.boot.loader.launch.PropertiesLauncher`, which has
no native override at all for its `loader.path`/classpath-resolution logic
(`getPaths()` / `Properties Launcher$Archives` / the `loader.path` property
read). That logic therefore runs as plain interpreted real JDK bytecode with
no CratonVM-specific shortcut — meaning if a bug exists here it is a genuine
interpreter/environment-level gap (e.g. a `System.getProperty("loader.path")`
read that doesn't see the value the test just set, a `File`/`Path`
resolution difference for the relative paths these tests use like `"jars/"`,
`"./jars/app.jar"`, `"nested-jars/app.jar!/./"`, or a classloader-ordering
difference that makes the JVM's already-loaded ambient classpath win over
the dynamically-constructed one) rather than a specific hardcoded shim like
the other three docs in this rerun. This was not traced further — no
`native-builtins`/`vm` file:line was identified as the culprit — and is
recorded here as an evidence-backed but unconfirmed hypothesis per the
task's own instruction to mark unverified root causes honestly rather than
guess a fix target.

## Affected classes

| module | class |
|---|---|
| loader/spring-boot-loader | org.springframework.boot.loader.launch.PropertiesLauncherTests |

(11 of its 14 failing tests: `testUserSpecifiedNestedJarPath`,
`testUserSpecifiedJarPath`, `testUserSpecifiedWildcardPath`,
`testUserSpecifiedRootOfJarPathWithDot`,
`testUserSpecifiedDirectoryContainingJarFileWithNestedArchives`,
`testUserSpecifiedClassLoader`, `testUserSpecifiedJarPathWithDot`,
`testUserSpecifiedJarFileWithNestedArchives`,
`testUserSpecifiedRootOfJarPath`,
`testUserSpecifiedRootOfJarPathWithDotAndJarPrefix`,
`classPathWithoutLoaderPathDefaultsToJarLauncherIncludes`.)
