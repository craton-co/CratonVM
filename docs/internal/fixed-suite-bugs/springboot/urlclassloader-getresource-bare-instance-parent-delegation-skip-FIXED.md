# Bare `java.net.URLClassLoader.getResource()` skipped parent delegation — FIXED

**Status: FIXED — 2026-07-19**

## Symptom

`module/spring-boot-web-server`'s `ServletComponentScanIntegrationTests
#indexedComponentsAreRegistered()` failed deterministically (3/3 repeat
runs) on current `dev`:

```
org.springframework.beans.factory.BeanDefinitionStoreException: I/O failure during classpath scanning
  at ClassPathScanningCandidateComponentProvider.addCandidateComponentsFromIndex(...)
Caused by: java.io.FileNotFoundException: class path resource [org/springframework/boot/web/server/servlet/context/testcomponents/listener/TestListener.class] cannot be opened because it does not exist
  at ClassPathResource.getInputStream(ClassPathResource.java:212)
  at ClassFileMetadataReader.parseClassModel/<init>
```

`TestListener.class` genuinely exists on disk at the expected path — this
is a classpath-resource-*resolution* bug, not a missing build artifact.
Confirmed via a matched-binary differential (two release builds from the
identical `origin/dev` tip `88fa839cc`, one with an unrelated concurrent
fix and one without — both reproduced this failure identically), so it
predates and is independent of that unrelated work.

## Root cause

The test wraps a thin resource overlay:

```java
try (URLClassLoader classLoader = new URLClassLoader(new URL[] { this.temp.toURI().toURL() },
        getClass().getClassLoader())) {
    this.context.setClassLoader(classLoader);
    ...
}
```

`this.temp` holds only a generated `../../../../apps/META-INF/spring.components` index file
— the actual `TestListener.class` bytes live on the **parent** (the real
test classpath). Spring's indexed classpath scan finds the index via the
child loader's own URL, then must open each listed class name as a
`ClassPathResource`, which requires `classLoader.getResourceAsStream(...)`
to fall through to the parent.

`native-builtins/src/classloader.rs`'s `cl_get_resource` — the native
backing `ClassLoader.getResource(String)`, and (via
`cl_get_resource_as_stream`'s existing delegation, see
[`urlclassloader-getresourceasstream-real-jdk-mode-dead-FIXED.md`](urlclassloader-getresourceasstream-real-jdk-mode-dead-FIXED.md))
also the effective path for `getResourceAsStream` — has a
`java.net.URLClassLoader`-specific branch. Before this fix:

```rust
if object_extends(ctx, this_ref, "java/net/URLClassLoader") {
    let class_name = ...;
    if is_builtin_loader_class(&class_name) {
        return ucl_find_resource(ctx, args);   // local-only, no parent
    }
    // ...parent-first delegation, then local...
}
```

`is_builtin_loader_class` matches the **literal string**
`"java/net/URLClassLoader"` (alongside `SecureClassLoader`,
`jdk/internal/loader/*`, `sun/misc/Launcher$*`) — so a **bare**,
directly-instantiated `new URLClassLoader(urls, parent)` (no subclass) hit
the early-return and got routed to `ucl_find_resource`, a strictly-local
lookup that can only ever see the receiver's own URL list (here, just the
temp dir's index file) and never consults the parent at all. A
user-defined **subclass** of `URLClassLoader` (e.g. Spring's
`FilteredClassLoader`) has its own distinct class name, doesn't match
`is_builtin_loader_class`, and correctly fell through to the
parent-delegation logic below — so this gap was specific to the bare-class
case.

This directly contradicts the real JDK contract: `ClassLoader.getResource()`
always delegates to the parent first, for **any** receiver, whether its own
class is literally `java.net.URLClassLoader` or a subclass. There is no
legitimate case (on a modern JDK) where a bare `URLClassLoader` instance
should skip parent delegation — `is_builtin_loader_class`'s other match
arms (`jdk/internal/loader/*`, `sun/misc/Launcher$*`) are dead code within
this specific `object_extends(.., "java/net/URLClassLoader")`-gated branch,
since JDK 9+'s actual internal loaders (`BuiltinClassLoader` and its
subclasses) do not themselves extend `URLClassLoader`.

The gate was introduced in `b52d6f6ef` ("fix Spring GraphQL lambda package
resources", 2026-07-18) specifically to give **subclasses** parent
delegation (`docs/internal/springboot/graphql-datafetcher-getpackage-null-npe-cluster-FIXED.md`);
its `is_builtin_loader_class(&class_name)` condition was an over-broad
proxy for "not a genuine user subclass" that incidentally also matched the
extremely common bare-`URLClassLoader` idiom this test uses. A sibling gap
in `cl_get_resource_as_stream` (the same `is_builtin_loader_class`
over-match, different function) was already fixed in
`6e1c323a7`/`urlclassloader-getresourceasstream-real-jdk-mode-dead-FIXED.md`,
whose fix doc explicitly noted `cl_get_resource`'s equivalent gate as
"already correct" at the time — this doc's gap must have been reintroduced
between then and now; not further bisected since the fix is unconditionally
correct on its own merits.

## Fix

`native-builtins/src/classloader.rs` (`cl_get_resource`): removed the
`is_builtin_loader_class` early-return in the
`object_extends(.., "java/net/URLClassLoader")` branch entirely. Every
`URLClassLoader` receiver (bare or subclass) now always attempts
parent-first delegation before falling back to its own local URL set —
matching the real JDK contract and mirroring the already-correct pattern
in `cl_get_resource_as_stream`'s own gate
(`object_extends(.., "java/net/URLClassLoader") || !is_builtin_loader_class(...)`).

## Verification

Binary built from `origin/dev` tip `deda4ba72` (2026-07-19) plus this fix,
worktree `CratonVM-scscan-classpath-regression-20260719`.

- `ServletComponentScanIntegrationTests`: 3/3 passing (up from 2/3), 3/3
  repeat runs, no flakiness.
- `module/spring-boot-web-server` full regression sweep: **29/29 passing**
  (up from 28/29 — this was the sole failure), including
  `MockWebEnvironmentServletComponentScanIntegrationTests` and
  `ServletComponentScanRegistrarTests`, both of which exercise adjacent
  classloader-delegation paths.
- `module/spring-boot-graphql` regression sweep (spot-check for the
  `b52d6f6ef` fix's original `FilteredClassLoader` subclass-delegation
  scenario, which this change's code path is directly adjacent to): 12/13
  passing (the 13th, `GraphQlQueryByExampleAutoConfigurationTests`, is an
  unrelated pre-existing "0 tests found" condition, 0 failures). All three
  classes named in that fix's own doc
  (`GraphQlRSocketAutoConfigurationTests`, `GraphQlWebFluxAutoConfigurationTests`,
  `GraphQlWebMvcAutoConfigurationTests`) pass.
- `cratonvm-native-builtins`'s `classloader` unit tests: 105/105 passing.

## Affected classes

- `module/spring-boot-web-server` | `ServletComponentScanIntegrationTests` | `indexedComponentsAreRegistered()`
