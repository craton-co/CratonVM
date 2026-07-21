# PropertiesLauncher JAR-loading regression cluster — FIXED

**Resolved 2026-07-21.** The earlier JIT-halfgap hypothesis was false: the
failure reproduces with `--nojit` and was caused by real-JDK collection and
class-loader boundary handling.

## Resolution

- `LaunchedClassLoader` now uses the normal real `ClassLoader` delegation
  bridge, including receiver-local URL lookup after parent delegation.
- Real `URLClassLoader` instances read the inherited `parent` relationship and
  preserve their constructor URLs for local resolution.
- `JarFileArchive.getClassPathUrls` now returns a real `ArrayList`, so a real
  `LinkedHashSet.addAll` observes its entries instead of silently treating the
  synthetic layout as empty.
- The archive bridge applies Spring Boot's supplied entry predicate and emits
  the correct URL protocol for directory class roots versus nested archives.

This closes the eight `demo.Application` `ClassNotFoundException` failures and
the four nested-root assertion failures in `PropertiesLauncherTests`.

## Verification

Using the unique release binary
`cratonvm-propertieslauncher-closure-final-20260721.exe` against the current
Spring Boot 4.1.0-SNAPSHOT fixture:

- `PropertiesLauncherTests`: **32/32 PASS** with `--nojit`.
- `PropertiesLauncherTests`: **32/32 PASS** with JIT enabled.
- Loader-subtree sweep (83 classes) had identical JIT/no-JIT outcomes:
  64 PASS, 14 existing failures, 4 empty abstract holders, and the existing
  `ZipContentTests` startup hang. No mode-specific regression or new failure
  was introduced by this change.

The document's separately catalogued merge-losses remain independent of this
PropertiesLauncher cluster; monitor-release-on-thread-death and Path dispatch
were already restored by subsequent `dev` fixes.
