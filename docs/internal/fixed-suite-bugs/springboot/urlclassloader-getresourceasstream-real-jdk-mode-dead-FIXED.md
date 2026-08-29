# `URLClassLoader.getResourceAsStream` dead in real-JDK mode — FIXED

**Status: FIXED — 2026-07-18**

## Symptom

Two Spring Boot `module/spring-boot-web-server` tests, previously verified
passing at an earlier `dev` commit, regressed again on later `dev` tip
(confirmed at `40678d0f5`, root-caused and fixed against `df1e9d14b`):

- `org.springframework.boot.web.server.servlet.context.ServletComponentScanIntegrationTests
  #indexedComponentsAreRegistered()`
- `org.springframework.boot.web.server.servlet.context.MockWebEnvironmentServletComponentScanIntegrationTests`
  (1 of 3 tests)

Both failed identically:

```
org.springframework.beans.factory.BeanDefinitionStoreException: I/O failure during classpath scanning
  at ClassPathScanningCandidateComponentProvider.addCandidateComponentsFromIndex(...)
Caused by: java.io.FileNotFoundException: class path resource [.../TestListener.class] cannot be opened because it does not exist
  at ClassPathResource.getInputStream(ClassPathResource.java:212)
  at ClassFileMetadataReader.parseClassModel/<init>
  at ClassFileMetadataReaderFactory.getMetadataReader
  at CachingMetadataReaderFactory.getMetadataReader (nested)
  at ClassPathScanningCandidateComponentProvider.addCandidateComponentsFromIndex(...)
```

Confirmed via `git stash` that this reproduces identically on an unmodified
`dev`-tip binary, ruling out interference from an unrelated concurrent fix
(`File(URI)` percent-decoding / `JarURLConnection` caching, see
[[project_staticresourcejarstests_jarurl_20260718]]).

## Root cause

`indexedComponentsAreRegistered()` writes a `../../../../apps/META-INF/spring.components`
index into a `@TempDir`, then does:

```java
try (URLClassLoader classLoader = new URLClassLoader(new URL[] { this.temp.toURI().toURL() },
        getClass().getClassLoader())) {
    this.context.setClassLoader(classLoader);
    ...
}
```

Spring's indexed classpath scan reads the index (found via the child
loader's own URL), then opens each listed class name as a
`ClassPathResource` — which must fall through to the **parent** classloader
(the real test classpath), since the child's only URL holds just the index
file.

Isolated with a standalone probe (`classLoader.getResource(name)` vs
`classLoader.getResourceAsStream(name)`): `getResource()` correctly resolved
the parent's `.class` file (matching real HotSpot exactly); `getResourceAsStream()`
returned `null` for the identical name on the identical loader.

Three independent, compounding gaps, all specific to `java.net.URLClassLoader`
declaring its own real-bytecode `getResourceAsStream` override (unlike
`getResource`/`getResources`/`findResource`, which it leaves to
`ClassLoader`/its own `findResource` extension point — confirmed via
`javap -p java.net.URLClassLoader`):

1. **The force-native gates never matched.** `force_native_over_real_jdk_bytecode`
   (`vm/src/runtime/interpreter.rs`) and the parallel `check_override`
   allow-list (`vm/src/vm/vm_exec.rs`) both had an entry forcing native
   dispatch for `getResourceAsStream` keyed on declaring class
   `java/lang/ClassLoader` — but since `URLClassLoader` declares its own
   override, native dispatch resolution never reaches that entry for a
   `URLClassLoader`-typed receiver. Real (broken) bytecode ran unforced.

2. **Even after fixing (1), the registry lookup still found nothing.** The
   only existing native registration for `java/net/URLClassLoader` +
   `getResourceAsStream` lived in
   `classloader::register_classloader_natives` /
   `servlet::register_s1_classloading` — both reachable only through
   `register_synthetic_overrides`, which is a **no-op stub** in the default
   real-JDK `cratonvm-cli` build (`vm/src/native/builtins.rs`,
   `#[cfg(feature = "synthetic-jdk")]`-gated). So this override was
   completely dead outside synthetic-JDK mode — confirmed empirically with
   `eprintln!` instrumentation: the force-check returned `true`, but
   `shared.native_methods.find(...)` returned `None`.

3. **The one delegation-aware implementation that DOES run in real-JDK mode**
   (`classloader::cl_get_resource_as_stream`, registered on the base
   `java/lang/ClassLoader` and reachable via `register_essential_natives`)
   explicitly excluded `java/net/URLClassLoader` from its parent-delegation
   path (`is_builtin_loader_class` groups the bare `URLClassLoader` class
   with `SecureClassLoader`/`jdk/internal/loader/*` as "builtin", routing it
   to a narrower `ctx.find_resource` fallback that doesn't see the
   dynamically-registered global URL walk `getResource` itself uses).

## Fix

- `native-builtins/src/lib.rs` (`register_essential_natives`): register
  `classloader::cl_get_resource_as_stream_essential` for
  `java/net/URLClassLoader` + `getResourceAsStream` directly, so it's active
  in the default real-JDK build.
- `native-builtins/src/classloader.rs` (`cl_get_resource_as_stream`): broaden
  the parent-delegation gate to `object_extends(.., "java/net/URLClassLoader")
  || !is_builtin_loader_class(...)`, mirroring the identical, already-correct
  gate in the sibling `cl_get_resource`.
- `vm/src/runtime/interpreter.rs` (`force_native_over_real_jdk_bytecode`) and
  `vm/src/vm/vm_exec.rs` (`check_override` allow-list, inside
  `invoke_on_class_shared_inner`): add a `java/net/URLClassLoader` +
  `getResourceAsStream` entry to each, alongside the existing
  `findClass`/`findResource`/`findResources`/`addURL`/`<init>` entries for
  the same class.

## Verification

Fixture: `apps/spring-boot` (Spring Boot 4.1.0-SNAPSHOT)
`module/spring-boot-web-server`, built on the Linux Azure host, run against
`cratonvm-scsindexed-20260718` built from worktree
`/data/data/wt-servletcomponentscan-indexedscan-20260718` (branch
`fix/servletcomponentscan-indexedscan-20260718`, `dev` @ `df1e9d14b`).

| Class | Mode | Result |
|---|---|---:|
| `ServletComponentScanIntegrationTests` | JIT | 3/3 PASS |
| `ServletComponentScanIntegrationTests` | `--nojit` | 3/3 PASS |
| `MockWebEnvironmentServletComponentScanIntegrationTests` | JIT | 3/3 PASS |
| `MockWebEnvironmentServletComponentScanIntegrationTests` | `--nojit` | 3/3 PASS |

**Regression sweep**: all 29 test classes in `module/spring-boot-web-server`
show no new failures relative to a pre-fix baseline. Two unrelated
pre-existing failures remain, each independently tracked:

- `WebServerSslBundleTests` (3 failed) —
  [`webserversslbundletests-pkcs12-mac-verification-failure.md`](../../known-issues/springboot/webserversslbundletests-pkcs12-mac-verification-failure.md).
- `ServletComponentScanRegistrarTests#processAheadOfTimeDoesNotRegisterServletComponentRegisteringPostProcessor()`
  (Spring AOT `TestCompiler` `CompilationException`, unrelated to classloader
  resource resolution) — flagged separately for follow-up (spawn_task
  `task_586d7cd9`), not fixed here.

This closure supersedes the "no active CratonVM defect" conclusion in
[`servletcomponentscanintegrationtests-registration-verified-FIXED.md`](servletcomponentscanintegrationtests-registration-verified-FIXED.md)
(verified at the earlier `dev` @ `cf3a44e2a` — the regression fixed here
landed on `dev` sometime between that commit and `40678d0f5`) and in
[`mockwebenvironmentservletcomponentscanintegrationtests-hang-FIXED.md`](mockwebenvironmentservletcomponentscanintegrationtests-hang-FIXED.md).
Both records are retained for their own evidence trails; this doc is the
current, authoritative closure for the `getResourceAsStream` regression.
