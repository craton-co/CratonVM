# `StaticResourceJarsTests` — JAR/URL-encoded-path resource lookup failures — FIXED

**Status: FIXED — 2026-07-18**

## Original symptom

Module `module/spring-boot-web-server`, class `StaticResourceJarsTests`, 3 failures:

- `closesJarFromNonCachedConnection()`: `AssertionError: Expecting code to raise a throwable.` — an exception the test expects never happens.
- `includeJarWithStaticResourcesWithUrlEncodedSpaces()` and `includeJarWithStaticResourcesWithPlusInItsPath()`: both `AssertionError: Expected size: 1 but was: 0 in: []` — static resources inside a JAR whose path contains URL-encoded spaces or a literal `+` are not found at all.

## Root causes and fixes

Two independent bugs in `native-builtins/src/net_phase_e.rs` /
`native-builtins/src/phases_late.rs`, both hit by `StaticResourceJars`
(`module/spring-boot-web-server/src/main/java/.../StaticResourceJars.java`).

**1. `new File(URI)` never percent-decoded the path (the two "was 0"
failures).** `StaticResourceJars.toFile(url)` calls `new File(url.toURI())`
for every `file:` URL. The native `File.<init>(Ljava/net/URI;)V` (`phases_late.rs`)
read the URI's path either from a real-JDK `URI.path` field by name or by
manually slicing the raw URI text, but never percent-decoded the result —
unlike real bytecode, which calls the `URI.getPath()` *accessor* (that method
decodes on read; the underlying `path` field itself stays raw/encoded). A jar
named `test resources.jar`/`test + resources.jar` produces a URI path
containing `%20`; the resulting `File` pointed at a literal `...%20....jar`
path that doesn't exist, so `isResourcesJar(file)` silently caught the
resulting `IOException` and returned `false` for every such JAR — this
matches [[reference_par_classpath_extension_uri_decode]]'s previously-flagged
but unfixed residual ("`new File(URI)` doesn't decode `%20`"). Fix: apply the
existing `net_phase_e::uri_percent_decode` helper (now `pub(crate)`) to the
resolved path before the `WinNTFileSystem.fromURIPath`-style transform.

**2. `JarURLConnection`/`JarFile` had no working caching or closed-state
tracking (the "no throwable" failure).** `closesJarFromNonCachedConnection`
expects `JarURLConnection.getJarFile()` to return the *same* `JarFile`
instance across calls (matching real
`sun.net.www.protocol.jar.JarURLConnection`, which caches it), so that
`StaticResourceJars` closing the jar via one reference is visible through
another. Three sub-fixes in `net_phase_e.rs`:

   - `JarURLConnection.getJarFile()` now caches the allocated `JarFile` in a
     spare carrier field (`HUC_JAR_FILE`, field 10) and returns the cached
     instance on subsequent calls instead of minting a fresh one every time.
   - `JarURLConnection.getUseCaches()`/`setUseCaches()` are declared on the
     base `java/net/URLConnection` (native dispatch keys on the resolved
     *declaring* class, so a naive override on `JarURLConnection` itself is
     silently never reached — confirmed by instrumenting the dispatch).
     Registered class-scoped overrides directly on `java/net/JarURLConnection`
     work because they're strictly more specific than the base-class no-op,
     with real state stored in `HUC_USE_CACHES` (reuses the `HUC_CODE`/field-2
     slot — unused by any jar-specific native). **Field 11 was tried first and
     discarded**: this carrier's real backing class reports a real
     total-field count of 11 (fields `0..=10`), so index 11 silently failed
     to persist writes across calls (confirmed with an isolated probe: the
     `set` landed, the very next `get` read back the unset default). Field 2
     sits inside the confirmed-persisting `0..=10` range.
   - `java/util/jar/JarFile.getComment()` (inherited from `ZipFile` in real
     bytecode, whose `ensureOpen()` throws `IllegalStateException("zip file
     closed")` post-`close()`) is now registered directly: our synthetic
     2-field `JarFile` (`path`=0, `manifest`=1; `close()` clears field 0) has
     no real `ZipFile` backing fields for that inherited bytecode to check,
     so it previously returned silently with no exception at all.

## Verification

Fixture: `apps/spring-boot` (Spring Boot 4.1.0-SNAPSHOT) `module/spring-boot-web-server`,
built on the Linux Azure host, run against a uniquely named CratonVM binary
(`cratonvm-staticresourcejars-20260718`) built from worktree
`/data/wt-staticresourcejars-20260718` (branch
`fix/staticresourcejars-jarurl-20260718`, based on `dev` @ `40678d0f5`).

`StaticResourceJarsTests`: 7/7 passing (1 `@DisabledForJreRange` skip, matching
HotSpot), in both JIT and `--nojit` (interpreter-only) mode.

**Regression sweep**: all 29 test classes in `module/spring-boot-web-server`
run clean except 4 pre-existing failures, each independently reproduced on an
unmodified baseline binary built from the same `dev` commit (`git stash` the
fix, rebuild, rerun) — confirming they are unrelated dev-tip drift, not
caused by this change:

- `WebServerSslBundleTests` (3 failed) — tracked by
  [`webserversslbundletests-pkcs12-mac-verification-failure.md`](../../known-issues/springboot/webserversslbundletests-pkcs12-mac-verification-failure.md).
- `SpringApplicationWebServerTests` (1 failed) — tracked by
  [`springapplicationwebservertests-environment-resolution-cluster.md`](../../known-issues/springboot/springapplicationwebservertests-environment-resolution-cluster.md).
- `ServletComponentScanIntegrationTests` / `MockWebEnvironmentServletComponentScanIntegrationTests`
  (1 failed each, `indexedComponentsAreRegistered()`,
  `FileNotFoundException` reading a classpath-index-referenced `.class`
  resource) — both classes were previously verified FIXED
  (`servletcomponentscanintegrationtests-registration-verified-FIXED.md`,
  `mockwebenvironmentservletcomponentscanintegrationtests-hang-FIXED.md`)
  at an earlier `dev` commit (`cf3a44e2a`); this is a **new regression** on
  current `dev` unrelated to `File(URI)`/`JarURLConnection` (no jar/URI
  resource on the classpath under test), flagged separately for follow-up.
